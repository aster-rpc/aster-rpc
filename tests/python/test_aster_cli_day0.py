from __future__ import annotations

import asyncio
import json
from argparse import Namespace

import pytest

from aster.trust.signing import generate_root_keypair
from aster_cli import access, aster_service, contract, identity, join, profile, publish


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


def test_build_publish_payload_matches_day0_schema():
    manifest = {
        "service": "TaskManager",
        "version": 1,
        "contract_id": "c" * 64,
        "canonical_encoding": "fory-xlang/0.15",
        "type_count": 0,
        "type_hashes": [],
        "method_count": 0,
        "methods": [],
        "serialization_modes": [],
        "scoped": "shared",
        "deprecated": False,
        "semver": None,
        "vcs_revision": None,
        "vcs_tag": None,
        "vcs_url": None,
        "changelog": None,
        "published_by": "",
        "published_at_epoch_ms": 0,
    }
    payload = publish._build_publish_payload(
        handle="alice-test",
        service_name="TaskManager",
        manifest=manifest,
        args=Namespace(
            endpoint_ttl="5m",
            token_ttl="5m",
            closed=False,
            private=False,
            description="Task queue",
            status="experimental",
            relay="",
            rate_limit=None,
            role=[],
        ),
        endpoint_id="node123",
    )

    assert payload["action"] == "publish"
    assert payload["handle"] == "alice-test"
    assert payload["service_name"] == "TaskManager"
    assert "visibility" not in payload
    assert payload["delegation"]["mode"] == "open"
    assert payload["endpoints"][0]["node_id"] == "node123"
    assert payload["contract_id"] != manifest["contract_id"]


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


def test_fetch_manifests_from_directory_ref(monkeypatch):
    captured = {}

    class FakePublicationClient:
        __module__ = "fakepkg.services.publication_service_v1"

        async def get_manifest(self, request):
            captured["request"] = request
            return type(
                "Result",
                (),
                {
                    "manifest_json": json.dumps(
                        {
                            "service": "ShellTestService",
                            "version": 1,
                            "contract_id": "abc123",
                            "methods": [],
                        }
                    )
                },
            )()

    class FakeRuntime:
        async def publication_client(self):
            return FakePublicationClient()

        async def close(self):
            captured["closed"] = True

    class FakeTypesModule:
        class GetManifestRequest:
            def __init__(self, *, handle, service_name):
                self.handle = handle
                self.service_name = service_name

    async def fake_open_aster_service(addr):
        captured["addr"] = addr
        return FakeRuntime()

    monkeypatch.setattr(contract.importlib, "import_module", lambda name: FakeTypesModule)
    monkeypatch.setattr("aster_cli.aster_service.open_aster_service", fake_open_aster_service)

    manifests = asyncio.run(
        contract._fetch_manifests_from_directory_ref("@alice/ShellTestService", "aster1test")
    )

    assert manifests["ShellTestService"]["contract_id"] == "abc123"
    assert captured["addr"] == "aster1test"
    assert captured["request"].handle == "alice"
    assert captured["request"].service_name == "ShellTestService"
    assert captured["closed"] is True


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


def test_cmd_access_grant_dispatch(monkeypatch, isolated_profile):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")
    captured = {}

    async def fake_grant_remote(args, *, handle):
        captured["handle"] = handle
        captured["service"] = args.service
        captured["consumer"] = args.consumer
        captured["role"] = args.role
        return 0

    monkeypatch.setattr(access, "_grant_remote", fake_grant_remote)

    rc = access.cmd_access_grant(
        Namespace(
            service="TaskManager",
            consumer="bob-test",
            role="consumer",
            scope="handle",
            scope_node_id=None,
            aster=None,
            root_key=None,
        )
    )

    assert rc == 0
    assert captured == {
        "handle": "alice-test",
        "service": "TaskManager",
        "consumer": "bob-test",
        "role": "consumer",
    }


def test_cmd_access_list_dispatch(monkeypatch, isolated_profile):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")
    captured = {}

    async def fake_list_remote(args, *, handle):
        captured["handle"] = handle
        captured["service"] = args.service
        captured["raw_json"] = args.raw_json
        return 0

    monkeypatch.setattr(access, "_list_remote", fake_list_remote)

    rc = access.cmd_access_list(
        Namespace(
            service="TaskManager",
            aster=None,
            root_key=None,
            raw_json=True,
        )
    )

    assert rc == 0
    assert captured == {
        "handle": "alice-test",
        "service": "TaskManager",
        "raw_json": True,
    }


def test_cmd_set_visibility_dispatch(monkeypatch, isolated_profile):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")
    captured = {}

    async def fake_set_visibility_remote(args, *, handle):
        captured["handle"] = handle
        captured["service"] = args.service
        captured["visibility"] = args.visibility
        return 0

    monkeypatch.setattr(publish, "_set_visibility_remote", fake_set_visibility_remote)

    rc = publish.cmd_set_visibility(
        Namespace(
            command="visibility",
            service="TaskManager",
            visibility="private",
            aster=None,
            root_key=None,
        )
    )

    assert rc == 0
    assert captured == {
        "handle": "alice-test",
        "service": "TaskManager",
        "visibility": "private",
    }


def test_cmd_update_service_dispatch(monkeypatch, isolated_profile):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")
    captured = {}

    async def fake_update_service_remote(args, *, handle):
        captured["handle"] = handle
        captured["service"] = args.service
        captured["description"] = args.description
        captured["status"] = args.status
        captured["replacement"] = args.replacement
        return 0

    monkeypatch.setattr(publish, "_update_service_remote", fake_update_service_remote)

    rc = publish.cmd_update_service(
        Namespace(
            command="update-service",
            service="TaskManager",
            description="New description",
            status="stable",
            replacement=None,
            aster=None,
            root_key=None,
        )
    )

    assert rc == 0
    assert captured == {
        "handle": "alice-test",
        "service": "TaskManager",
        "description": "New description",
        "status": "stable",
        "replacement": None,
    }


def test_cmd_access_delegation_dispatch(monkeypatch, isolated_profile):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")
    captured = {}

    async def fake_delegation_remote(args, *, handle):
        captured["handle"] = handle
        captured["service"] = args.service
        captured["closed"] = args.closed
        captured["roles"] = args.role
        return 0

    monkeypatch.setattr(access, "_delegation_remote", fake_delegation_remote)

    rc = access.cmd_access_delegation(
        Namespace(
            access_command="delegation",
            service="TaskManager",
            open=False,
            closed=True,
            token_ttl="10m",
            rate_limit=None,
            role=["consumer"],
            aster=None,
            root_key=None,
        )
    )

    assert rc == 0
    assert captured == {
        "handle": "alice-test",
        "service": "TaskManager",
        "closed": True,
        "roles": ["consumer"],
    }


def test_cmd_access_public_private_dispatch(monkeypatch):
    captured = {}

    def fake_set_visibility(args):
        captured["service"] = args.service
        captured["visibility"] = args.visibility
        return 0

    monkeypatch.setattr("aster_cli.publish.cmd_set_visibility", fake_set_visibility)

    rc = access.cmd_access_public_private(
        Namespace(
            access_command="private",
            service="TaskManager",
            aster="aster1example",
            root_key="/tmp/root.key",
        )
    )

    assert rc == 0
    assert captured == {
        "service": "TaskManager",
        "visibility": "private",
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


def test_build_status_lines_include_remote_details(isolated_profile):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified", email="alice@example.com")
    state = join.get_local_identity_state()
    state["remote"] = {
        "email_masked": "a***@example.com",
        "display_name": "Alice",
        "services_published": 2,
        "recovery_codes_remaining": 5,
    }

    lines = join.build_status_lines(state)

    assert "Remote: reachable" in lines
    assert "Remote email: a***@example.com" in lines
    assert "Display name: Alice" in lines
    assert "Published services: 2" in lines
    assert "Recovery codes remaining: 5" in lines


def test_producer_token_store_round_trip(isolated_profile):
    identity_path = isolated_profile[0] / "node.identity"
    identity.save_identity(identity_path, {"node": {"endpoint_id": "node123"}, "peers": []})

    token_path = publish.store_producer_token(
        "TaskManager",
        "tok_123",
        contract_id="abc123",
        identity_file=str(identity_path),
    )

    assert token_path.exists()
    loaded = identity.load_identity(identity_path)
    assert loaded["published_services"]["TaskManager"]["producer_token"] == "tok_123"
    assert publish.load_producer_token("TaskManager", identity_file=str(identity_path)) == "tok_123"
    assert publish.remove_producer_token("TaskManager", identity_file=str(identity_path)) is True
    assert publish.load_producer_token("TaskManager", identity_file=str(identity_path)) is None
    assert identity.load_identity(identity_path)["peers"] == []


def test_publish_remote_stores_producer_token(monkeypatch, isolated_profile, capsys):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified")
    identity_path = isolated_profile[0] / "node.identity"
    identity.save_identity(identity_path, {"node": {"endpoint_id": "node123"}, "peers": [{"name": "dev"}]})

    captured = {"visibility_calls": 0}

    class FakePublicationClient:
        async def publish(self, request):
            captured["publish_request"] = request
            return type(
                "Result",
                (),
                {
                    "producer_token": "prod-token-xyz",
                    "first_publish": False,
                },
            )()

        async def set_visibility(self, request):
            captured["visibility_calls"] += 1

    class FakeRuntime:
        async def publication_client(self):
            return FakePublicationClient()

        def signed_request(self, envelope):
            return envelope

        async def close(self):
            captured["closed"] = True

    async def fake_open_aster_service(addr):
        captured["addr"] = addr
        return FakeRuntime()

    monkeypatch.setattr(publish, "open_aster_service", fake_open_aster_service)
    monkeypatch.setattr(
        publish,
        "build_signed_envelope",
        lambda payload, root_key_file=None: type("Envelope", (), {"payload": payload})(),
    )

    rc = asyncio.run(
        publish._publish_remote(
            Namespace(
                aster="aster1example",
                root_key=None,
                identity_file=str(identity_path),
                private=False,
                public=False,
                endpoint_ttl="5m",
                token_ttl="5m",
                closed=False,
                description="Task queue",
                status="experimental",
                relay="",
                rate_limit=None,
                role=[],
            ),
            handle="alice-test",
            service_name="TaskManager",
            manifest_dict={
                "service": "TaskManager",
                "version": 1,
                "contract_id": "c" * 64,
                "canonical_encoding": "fory-xlang/0.15",
                "type_count": 0,
                "type_hashes": [],
                "method_count": 0,
                "methods": [],
                "serialization_modes": [],
                "scoped": "shared",
                "deprecated": False,
                "semver": None,
                "vcs_revision": None,
                "vcs_tag": None,
                "vcs_url": None,
                "changelog": None,
                "published_by": "",
                "published_at_epoch_ms": 0,
            },
            endpoint_id="node123",
        )
        )

    out = capsys.readouterr().out
    assert rc == 0
    assert "Stored producer token" in out
    stored = identity.load_identity(identity_path)
    assert stored["published_services"]["TaskManager"]["producer_token"] == "prod-token-xyz"
    assert stored["peers"][0]["published_services"]["TaskManager"]["contract_id"] == captured["publish_request"].payload["contract_id"]


def test_unpublish_remote_removes_producer_token(monkeypatch, isolated_profile, capsys):
    _write_profile(root_pubkey="ab" * 32, handle="alice-test", handle_status="verified", published_services="TaskManager")
    identity_path = isolated_profile[0] / "node.identity"
    identity.save_identity(identity_path, {"node": {"endpoint_id": "node123"}, "peers": [{"name": "dev"}]})
    publish.store_producer_token(
        "TaskManager",
        "prod-token-xyz",
        contract_id="c" * 64,
        identity_file=str(identity_path),
    )

    class FakePublicationClient:
        async def unpublish(self, request):
            return None

    class FakeRuntime:
        async def publication_client(self):
            return FakePublicationClient()

        def signed_request(self, envelope):
            return envelope

        async def close(self):
            return None

    async def fake_open_aster_service(addr):
        return FakeRuntime()

    monkeypatch.setattr(publish, "open_aster_service", fake_open_aster_service)
    monkeypatch.setattr(
        publish,
        "build_signed_envelope",
        lambda payload, root_key_file=None: type("Envelope", (), {"payload": payload})(),
    )

    rc = asyncio.run(
        publish._unpublish_remote(
            Namespace(
                aster="aster1example",
                root_key=None,
                identity_file=str(identity_path),
                service="TaskManager",
            ),
            handle="alice-test",
        )
    )

    out = capsys.readouterr().out
    assert rc == 0
    assert "Removed stored producer token." in out
    assert publish.load_producer_token("TaskManager", identity_file=str(identity_path)) is None


def test_identity_load_exposes_published_services_on_peers(tmp_path):
    identity_path = tmp_path / ".aster-identity"
    identity.save_identity(
        identity_path,
        {
            "node": {"endpoint_id": "node123"},
            "peers": [{"name": "dev"}],
            "published_services": {
                "TaskManager": {
                    "producer_token": "tok_123",
                    "contract_id": "abc123",
                    "service_name": "TaskManager",
                }
            },
        },
    )

    loaded = identity.load_identity(identity_path)

    assert loaded["published_services"]["TaskManager"]["producer_token"] == "tok_123"
    assert loaded["peers"][0]["published_services"]["TaskManager"]["contract_id"] == "abc123"
