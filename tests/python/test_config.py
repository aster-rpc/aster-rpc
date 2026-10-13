"""AsterConfig: keyword / env / TOML loading through the Rust ``aster::AsterConfig``.

Covers every knob in the ``docs/aster-rust-getstarted.md`` section 2.2 table:
each one set by keyword, by ``ASTER_*`` env and by ``aster.toml`` must arrive
in the Rust config unchanged (``_native().to_dict()``), and the Python
dataclass must mirror it.
"""

import base64
import dataclasses
import json
import os
import textwrap

import pytest

from aster import AsterConfig, EnrollmentConfig, PublicKey, SecretKey


@pytest.fixture(autouse=True)
def _clean_env(monkeypatch, tmp_path):
    for key in list(os.environ):
        if key.startswith("ASTER_"):
            monkeypatch.delenv(key, raising=False)
    # Keep a stray ./.aster-identity from being auto-detected.
    monkeypatch.chdir(tmp_path)


def _write(tmp_path, name: str, content: str):
    p = tmp_path / name
    p.write_text(textwrap.dedent(content))
    return p


KEY = bytes(range(32))
PUB = bytes(range(32, 64))

# Every knob, with a non-default value: (field, python value, env value, toml line).
KNOBS = [
    ("relay_mode", "disabled", "disabled", 'relay_mode = "disabled"'),
    ("secret_key", KEY, KEY.hex(), f'secret_key = "{KEY.hex()}"'),
    ("secret_key_file", "/keys/node.key", "/keys/node.key", 'secret_key_file = "/keys/node.key"'),
    ("bind_addr", "0.0.0.0:9000", "0.0.0.0:9000", 'bind_addr = "0.0.0.0:9000"'),
    ("local_discovery", True, "true", "local_discovery = true"),
    ("enable_monitoring", True, "1", "enable_monitoring = true"),
    ("enable_hooks", True, "yes", "enable_hooks = true"),
    ("hook_timeout_ms", 1234, "1234", "hook_timeout_ms = 1234"),
    ("hook_failure_mode", "fail_closed", "fail_closed", 'hook_failure_mode = "fail_closed"'),
    ("clear_ip_transports", True, "true", "clear_ip_transports = true"),
    ("clear_relay_transports", True, "true", "clear_relay_transports = true"),
    ("portmapper_config", "disabled", "disabled", 'portmapper_config = "disabled"'),
    ("proxy_url", "http://proxy:3128", "http://proxy:3128", 'proxy_url = "http://proxy:3128"'),
    ("proxy_from_env", True, "on", "proxy_from_env = true"),
    ("transport_max_concurrent_bidi_streams", 11, "11", "transport_max_concurrent_bidi_streams = 11"),
    ("transport_max_concurrent_uni_streams", 12, "12", "transport_max_concurrent_uni_streams = 12"),
    ("transport_stream_receive_window", 13, "13", "transport_stream_receive_window = 13"),
    ("transport_receive_window", 14, "14", "transport_receive_window = 14"),
    ("transport_send_window", 15, "15", "transport_send_window = 15"),
    ("transport_max_idle_timeout_ms", 16, "16", "transport_max_idle_timeout_ms = 16"),
    ("transport_keep_alive_interval_ms", 17, "17", "transport_keep_alive_interval_ms = 17"),
    ("transport_initial_mtu", 1400, "1400", "transport_initial_mtu = 1400"),
    ("transport_datagram_receive_buffer_size", 19, "19", "transport_datagram_receive_buffer_size = 19"),
    ("transport_datagram_send_buffer_size", 20, "20", "transport_datagram_send_buffer_size = 20"),
    ("transport_send_fairness", False, "false", "transport_send_fairness = false"),
    ("transport_enable_segmentation_offload", False, "0", "transport_enable_segmentation_offload = false"),
]
# Non-[network] knobs: (field, python value, env var, env value, section, toml line).
OTHER_KNOBS = [
    ("storage_path", "/var/lib/aster", "ASTER_STORAGE_PATH", "/var/lib/aster", "storage", 'path = "/var/lib/aster"'),
    ("gc_interval_ms", 60000, "ASTER_GC_INTERVAL_MS", "60000", "storage", "gc_interval_ms = 60000"),
    ("identity_file", "/creds/.aster-identity", "ASTER_IDENTITY_FILE", "/creds/.aster-identity", "identity", 'file = "/creds/.aster-identity"'),
    ("root_pubkey", PUB, "ASTER_ROOT_PUBKEY", PUB.hex(), "trust", f'root_pubkey = "{PUB.hex()}"'),
    ("root_pubkey_file", "/keys/root.pub", "ASTER_ROOT_PUBKEY_FILE", "/keys/root.pub", "trust", 'root_pubkey_file = "/keys/root.pub"'),
    ("enrollment_credential_file", "/c.cred", "ASTER_ENROLLMENT_CREDENTIAL", "/c.cred", "trust", 'enrollment_credential = "/c.cred"'),
    ("enrollment_credential_chain", "aster-chain-v1~x", "ASTER_ENROLLMENT_CREDENTIAL_CHAIN", "aster-chain-v1~x", "trust", 'enrollment_credential_chain = "aster-chain-v1~x"'),
    ("next_root_commitment", "ab" * 32, "ASTER_NEXT_ROOT_COMMITMENT", "ab" * 32, "trust", f'next_root_commitment = "{"ab" * 32}"'),
    ("root_seq", 3, "ASTER_ROOT_SEQ", "3", "trust", "root_seq = 3"),
    ("root_rotations", ("s1", "s2"), "ASTER_ROOT_ROTATIONS", "s1, s2", "trust", 'root_rotations = ["s1", "s2"]'),
    ("endpoint_addr", "aster1xyz", "ASTER_ENDPOINT_ADDR", "aster1xyz", "connect", 'endpoint_addr = "aster1xyz"'),
    ("log_format", "json", "ASTER_LOG_FORMAT", "JSON", "logging", 'format = "json"'),
    ("log_level", "debug", "ASTER_LOG_LEVEL", "debug", "logging", 'level = "debug"'),
    ("log_mask", False, "ASTER_LOG_MASK", "false", "logging", "mask = false"),
]


def _rust(cfg: AsterConfig) -> dict:
    """The Rust config's view, in the dataclass spelling (lists as tuples,
    ``enrollment`` as an ``EnrollmentConfig``)."""
    out = {
        k: tuple(v) if isinstance(v, list) else v
        for k, v in cfg._native().to_dict().items()
    }
    out["enrollment"] = EnrollmentConfig._from_native(out["enrollment"])
    return out


# ---------------------------------------------------------------------------
# Defaults / dataclass behaviour
# ---------------------------------------------------------------------------

def test_defaults_match_rust_defaults():
    cfg = AsterConfig()
    assert cfg == AsterConfig.from_env()
    assert cfg.to_dict() == _rust(cfg)
    assert cfg.relay_mode == "default"
    assert cfg.relay_urls == ()
    assert cfg.storage_path is None and not cfg.is_persistent
    assert cfg.secret_key is None and cfg.secret_key_file is None
    assert cfg.hook_timeout_ms == 5000
    assert cfg.hook_failure_mode == "fail_open"
    assert cfg.enrollment == EnrollmentConfig()
    assert not cfg.is_enrollment_enabled
    assert cfg.log_format == "text" and cfg.log_level == "info" and cfg.log_mask is True


def test_frozen_and_replace():
    cfg = AsterConfig()
    with pytest.raises(dataclasses.FrozenInstanceError):
        cfg.storage_path = "/x"  # type: ignore[misc]
    new = cfg.replace(storage_path="/x", enable_hooks=True)
    assert new.storage_path == "/x" and new.enable_hooks is True
    assert cfg.storage_path is None
    assert new._sources["storage_path"] == "code"
    with pytest.raises(ValueError):
        cfg.replace(hook_failure_mode="sometimes")


def test_unknown_field_rejected():
    with pytest.raises(TypeError):
        AsterConfig(no_such_knob=1)  # type: ignore[call-arg]


def test_secret_key_not_in_repr():
    cfg = AsterConfig(secret_key=KEY)
    assert KEY.hex() not in repr(cfg)
    assert "secret_key=" not in repr(cfg)


def test_typed_keys_accepted_and_normalised():
    sk = SecretKey.from_bytes(KEY)
    cfg = AsterConfig(secret_key=sk, root_pubkey=PublicKey.from_bytes(PUB))
    assert cfg.secret_key == KEY and cfg.root_pubkey == PUB
    assert AsterConfig(secret_key=KEY.hex(), root_pubkey=PUB.hex()) == cfg


# ---------------------------------------------------------------------------
# Every knob: kwargs / env / TOML -> Rust
# ---------------------------------------------------------------------------

@pytest.mark.parametrize("field,value,_env,_toml", KNOBS, ids=[k[0] for k in KNOBS])
def test_kwarg_roundtrip(field, value, _env, _toml):
    cfg = AsterConfig(**{field: value})
    assert getattr(cfg, field) == value
    assert _rust(cfg)[field] == value
    assert cfg.to_dict() == _rust(cfg)


def test_all_kwargs_together_roundtrip():
    kwargs = {k[0]: k[1] for k in KNOBS}
    kwargs.update({k[0]: k[1] for k in OTHER_KNOBS})
    kwargs["alpns"] = (b"my/1", b"my/2")
    cfg = AsterConfig(**kwargs)
    rust = _rust(cfg)
    for field, value in kwargs.items():
        assert rust[field] == value, field
    # Rebuilding from the Rust view is the identity.
    assert AsterConfig(**cfg.to_dict()) == cfg


@pytest.mark.parametrize("field,value,env,_toml", KNOBS, ids=[k[0] for k in KNOBS])
def test_env_roundtrip(monkeypatch, field, value, env, _toml):
    monkeypatch.setenv(f"ASTER_{field.upper()}", env)
    cfg = AsterConfig.from_env()
    assert getattr(cfg, field) == value
    assert _rust(cfg)[field] == value
    assert cfg._sources[field] == f"ASTER_{field.upper()}"


@pytest.mark.parametrize(
    "field,value,var,env,_s,_t", OTHER_KNOBS, ids=[k[0] for k in OTHER_KNOBS]
)
def test_env_roundtrip_other_sections(monkeypatch, field, value, var, env, _s, _t):
    monkeypatch.setenv(var, env)
    cfg = AsterConfig.from_env()
    assert getattr(cfg, field) == value
    assert cfg._sources[field] == var


@pytest.mark.parametrize("field,value,_env,toml", KNOBS, ids=[k[0] for k in KNOBS])
def test_toml_roundtrip(tmp_path, field, value, _env, toml):
    p = _write(tmp_path, "aster.toml", f"[network]\n{toml}\n")
    cfg = AsterConfig.from_file(p)
    assert getattr(cfg, field) == value
    assert _rust(cfg)[field] == value
    assert cfg._sources[field] == f"{p} [network]"


@pytest.mark.parametrize(
    "field,value,_v,_e,section,toml", OTHER_KNOBS, ids=[k[0] for k in OTHER_KNOBS]
)
def test_toml_roundtrip_other_sections(tmp_path, field, value, _v, _e, section, toml):
    p = _write(tmp_path, "aster.toml", f"[{section}]\n{toml}\n")
    cfg = AsterConfig.from_file(p)
    assert getattr(cfg, field) == value
    assert cfg._sources[field] == f"{p} [{section}]"


def test_env_overrides_file(tmp_path, monkeypatch):
    p = _write(tmp_path, "aster.toml", """
        [network]
        bind_addr = "0.0.0.0:1"
        enable_hooks = true
        [storage]
        path = "/from/file"
    """)
    monkeypatch.setenv("ASTER_BIND_ADDR", "0.0.0.0:2")
    cfg = AsterConfig.from_file(p)
    assert cfg.bind_addr == "0.0.0.0:2"
    assert cfg.enable_hooks is True
    assert cfg.storage_path == "/from/file"
    assert cfg._sources["bind_addr"] == "ASTER_BIND_ADDR"
    assert cfg._sources["enable_hooks"].endswith("[network]")
    assert cfg._sources["storage_path"].endswith("[storage]")


def test_gc_interval_zero_disables(monkeypatch):
    monkeypatch.setenv("ASTER_GC_INTERVAL_MS", "0")
    assert AsterConfig.from_env().gc_interval_ms is None
    assert AsterConfig(gc_interval_ms=0).gc_interval_ms is None


# ---------------------------------------------------------------------------
# Relay mode
# ---------------------------------------------------------------------------

def test_relay_urls_select_custom():
    cfg = AsterConfig(relay_urls=["https://r1.example", "https://r2.example"])
    assert cfg.relay_mode == "custom"
    assert cfg.relay_urls == ("https://r1.example", "https://r2.example")
    assert AsterConfig(relay_mode="custom", relay_urls=["https://r1.example"]).relay_mode == "custom"


def test_relay_bare_url_is_custom():
    cfg = AsterConfig(relay_mode="https://relay.example")
    assert cfg.relay_mode == "custom"
    assert cfg.relay_urls == ("https://relay.example",)


def test_relay_conflicts_rejected():
    with pytest.raises(ValueError):
        AsterConfig(relay_mode="disabled", relay_urls=["https://r.example"])
    with pytest.raises(ValueError):
        AsterConfig(relay_mode="custom")


def test_relay_urls_env_and_toml(tmp_path, monkeypatch):
    monkeypatch.setenv("ASTER_RELAY_URLS", "https://a.example, https://b.example")
    cfg = AsterConfig.from_env()
    assert cfg.relay_mode == "custom"
    assert cfg.relay_urls == ("https://a.example", "https://b.example")
    monkeypatch.delenv("ASTER_RELAY_URLS")
    p = _write(tmp_path, "aster.toml", """
        [network]
        relay_urls = ["https://c.example"]
    """)
    assert AsterConfig.from_file(p).relay_urls == ("https://c.example",)


# ---------------------------------------------------------------------------
# Secret key spellings and errors
# ---------------------------------------------------------------------------

def test_secret_key_env_hex_or_base64(monkeypatch):
    monkeypatch.setenv("ASTER_SECRET_KEY", base64.b64encode(KEY).decode())
    assert AsterConfig.from_env().secret_key == KEY
    monkeypatch.setenv("ASTER_SECRET_KEY", KEY.hex())
    assert AsterConfig.from_env().secret_key == KEY


def test_secret_key_env_empty_clears_file_value(tmp_path, monkeypatch):
    p = _write(tmp_path, "aster.toml", f'[network]\nsecret_key = "{KEY.hex()}"\n')
    monkeypatch.setenv("ASTER_SECRET_KEY", "")
    assert AsterConfig.from_file(p).secret_key is None


@pytest.mark.parametrize(
    "var,value",
    [
        ("ASTER_SECRET_KEY", "not-a-key!!"),
        ("ASTER_ENABLE_HOOKS", "maybe"),
        ("ASTER_HOOK_TIMEOUT_MS", "soon"),
        ("ASTER_HOOK_FAILURE_MODE", "sometimes"),
        ("ASTER_PORTMAPPER_CONFIG", "auto"),
        ("ASTER_TRANSPORT_INITIAL_MTU", "99999999"),
        ("ASTER_ROOT_PUBKEY", "abcd"),
        ("ASTER_RELAY_MODE", "staging"),
    ],
)
def test_env_invalid_values(monkeypatch, var, value):
    monkeypatch.setenv(var, value)
    if var == "ASTER_RELAY_MODE":
        monkeypatch.setenv("ASTER_RELAY_URLS", "https://r.example")
    with pytest.raises(ValueError, match=var.split("_", 1)[0]):
        AsterConfig.from_env()


def test_kwarg_invalid_values():
    with pytest.raises(ValueError):
        AsterConfig(secret_key=b"short")
    with pytest.raises(ValueError):
        AsterConfig(portmapper_config="auto")
    with pytest.raises(TypeError):
        AsterConfig(enable_hooks="yes")  # type: ignore[arg-type]
    with pytest.raises(OverflowError):
        AsterConfig(transport_initial_mtu=1 << 20)


def test_toml_errors(tmp_path):
    with pytest.raises(FileNotFoundError):
        AsterConfig.from_file(tmp_path / "missing.toml")
    bad = _write(tmp_path, "bad.toml", "[network\nbind_addr = 1\n")
    with pytest.raises(ValueError, match="invalid TOML"):
        AsterConfig.from_file(bad)
    wrong_type = _write(tmp_path, "wrong.toml", '[network]\nenable_hooks = "yes"\n')
    with pytest.raises(ValueError, match="enable_hooks"):
        AsterConfig.from_file(wrong_type)


# ---------------------------------------------------------------------------
# Root public key resolution (Rust)
# ---------------------------------------------------------------------------

def test_root_pubkey_file_hex_and_json(tmp_path):
    hex_file = _write(tmp_path, "root.pub", PUB.hex() + "\n")
    assert AsterConfig(root_pubkey_file=str(hex_file)).resolve_root_pubkey() == PUB
    json_file = _write(tmp_path, "root.json", json.dumps({"public_key": PUB.hex()}))
    assert AsterConfig(root_pubkey_file=str(json_file)).resolve_root_pubkey() == PUB
    missing = AsterConfig(root_pubkey_file=str(tmp_path / "nope"))
    assert missing.resolve_root_pubkey() is None


def test_root_pubkey_inline_and_file_must_agree(tmp_path):
    f = _write(tmp_path, "root.pub", PUB.hex())
    assert AsterConfig(root_pubkey=PUB, root_pubkey_file=str(f)).resolve_root_pubkey() == PUB
    with pytest.raises(ValueError, match="one deployment has one"):
        AsterConfig(root_pubkey=bytes(32), root_pubkey_file=str(f)).resolve_root_pubkey()


def test_root_pubkey_file_malformed(tmp_path):
    f = _write(tmp_path, "root.pub", "zz")
    with pytest.raises(ValueError):
        AsterConfig(root_pubkey_file=str(f)).resolve_root_pubkey()


# ---------------------------------------------------------------------------
# Secret key file (the one node-key format)
# ---------------------------------------------------------------------------

def test_secret_key_file_created_then_reloaded(tmp_path):
    path = tmp_path / "keys" / "node.key"
    cfg = AsterConfig(secret_key_file=str(path))
    key = cfg.resolve_secret_key()
    assert key is not None
    text = path.read_text()
    assert text == key.to_hex() + "\n" and len(key.to_hex()) == 64
    if os.name == "posix":
        assert (path.stat().st_mode & 0o777) == 0o600
    # Second resolve reuses the file.
    assert cfg.resolve_secret_key() == key
    assert SecretKey.read_file(str(path)) == key


def test_secret_key_file_written_from_inline_key(tmp_path):
    path = tmp_path / "node.key"
    cfg = AsterConfig(secret_key=KEY, secret_key_file=str(path))
    assert cfg.resolve_secret_key().to_bytes() == KEY
    assert path.read_text().strip() == KEY.hex()


def test_secret_key_file_conflict_with_inline_key(tmp_path):
    path = tmp_path / "node.key"
    SecretKey.from_bytes(KEY).write_file(str(path))
    cfg = AsterConfig(secret_key=bytes(32), secret_key_file=str(path))
    with pytest.raises(ValueError, match="different keys"):
        cfg.resolve_secret_key()


def test_secret_key_file_malformed(tmp_path):
    path = _write(tmp_path, "node.key", json.dumps({"secret_key": KEY.hex()}))
    with pytest.raises(ValueError, match="secret key file"):
        AsterConfig(secret_key_file=str(path)).resolve_secret_key()


def test_secret_key_api(tmp_path):
    k = SecretKey.generate()
    assert repr(k) == "SecretKey(<redacted>)"
    assert SecretKey.from_hex(k.to_hex()) == k
    assert SecretKey.from_bytes(k.to_bytes()) == k
    assert k.public_key().node_id() == k.node_id()
    path = tmp_path / "k.key"
    k.write_file(str(path))
    with pytest.raises(ValueError):
        k.write_file(str(path))  # never overwrites
    assert SecretKey.load_or_generate(str(path)) == k
    assert SecretKey.read_file_if_exists(str(tmp_path / "none")) is None
    fresh = SecretKey.load_or_generate(str(tmp_path / "fresh.key"))
    assert SecretKey.read_file(str(tmp_path / "fresh.key")) == fresh


def test_keygen_producer_writes_key_file(tmp_path, capsys):
    from aster_cli.keygen import _keygen_producer

    class Args:
        out = str(tmp_path / "node.key")

    assert _keygen_producer(Args()) == 0
    key = SecretKey.read_file(Args.out)
    assert str(key.node_id()) in capsys.readouterr().out
    assert _keygen_producer(Args()) == 1  # refuses to overwrite


# ---------------------------------------------------------------------------
# .aster-identity credential file
# ---------------------------------------------------------------------------

def _identity(tmp_path, secret: str):
    return _write(tmp_path, ".aster-identity", f"""
        [node]
        secret_key = "{secret}"

        [[peers]]
        name = "edge"
        role = "producer"
        root_pubkey = "{PUB.hex()}"

        [[peers]]
        name = "app"
        role = "consumer"
        root_pubkey = "{PUB.hex()}"
        nonce = "{'00' * 32}"
    """)


@pytest.mark.parametrize("encoding", ["base64", "hex"])
def test_load_identity_secret_key_and_peer(tmp_path, encoding):
    secret = base64.b64encode(KEY).decode() if encoding == "base64" else KEY.hex()
    path = _identity(tmp_path, secret)
    cfg = AsterConfig(identity_file=str(path))
    assert cfg.resolve_identity_path() == str(path)
    key, peer = cfg.load_identity(role="consumer")
    assert key == KEY
    assert peer["name"] == "app" and peer["nonce"] == "00" * 32
    _, peer = cfg.load_identity(peer_name="edge")
    assert peer["role"] == "producer"
    _, peer = cfg.load_identity()
    assert peer["name"] == "edge"


def test_load_identity_autodetects_cwd(tmp_path):
    _identity(tmp_path, KEY.hex())
    key, _ = AsterConfig().load_identity()
    assert key == KEY


def test_load_identity_missing(tmp_path):
    cfg = AsterConfig(identity_file=str(tmp_path / "none"))
    assert cfg.load_identity() == (None, None)


def test_load_identity_malformed_key(tmp_path):
    path = _identity(tmp_path, "nope")
    with pytest.raises(ValueError):
        AsterConfig(identity_file=str(path)).load_identity()


# ---------------------------------------------------------------------------
# print_config
# ---------------------------------------------------------------------------

def test_print_config_masks_and_reports_sources(monkeypatch, capsys):
    monkeypatch.setenv("ASTER_SECRET_KEY", KEY.hex())
    cfg = AsterConfig.from_env()
    text = cfg.print_config()
    assert KEY.hex() not in text
    assert "****..." + KEY.hex()[-8:] in text
    assert "ASTER_SECRET_KEY" in text
    data = json.loads(cfg.print_config(json=True))
    assert data["network"]["secret_key"]["source"] == "ASTER_SECRET_KEY"
    assert data["storage"]["path"]["source"] == "default"


# ---------------------------------------------------------------------------
# [enrollment] (section 15.3 of the enrollment design): names, enablement
# ---------------------------------------------------------------------------

ENROLLMENT_TOML = """
[trust]
root_pubkey = "{pub}"
enrollment_credential = "server.cred"

[enrollment]
audience = "mission-control"
fleet_namespace = "{ns}"
state_path = "/var/lib/mc/enrollment.json"
replicas = ["aster1abc", "bb"]
policy = {{ all_of = [{{ scope = "ops" }}, "tolerate_stale_policy"] }}
activation_leeway_secs = 30
renew_from = "dd"
synchronize = false

[enrollment.limits]
handshake_timeout_ms = 5000
sync_timeout_ms = 2000

[enrollment.roles]
attribute = ""
scopes_as_roles = true
include_attributes = false
[enrollment.roles.scopes]
"fleet.ops" = ["ops.status", "ops.logs"]
[enrollment.roles.tags]
admin = "ops.admin, ops.logs"

[enrollment.issuer]
delegation_file = "issuer.chain"
lifetime = "6h"
"""


def test_enrollment_toml_section_parses_through_rust(tmp_path):
    ns = "cd" * 32
    p = _write(tmp_path, "aster.toml", ENROLLMENT_TOML.format(pub=PUB.hex(), ns=ns))
    cfg = AsterConfig.from_file(p)
    e = cfg.enrollment
    assert cfg.enrollment_credential_file == "server.cred"
    assert cfg.is_enrollment_enabled
    assert e.audience == "mission-control"
    assert e.fleet_namespace == ns
    assert e.state_path == "/var/lib/mc/enrollment.json"
    assert e.replicas == ("aster1abc", "bb")
    assert e.policy == {"all_of": [{"scope": "ops"}, "tolerate_stale_policy"]}
    assert e.activation_leeway_secs == 30
    assert e.renew_from == "dd" and e.synchronize is False
    assert e.limits.handshake_timeout_ms == 5000 and e.limits.sync_timeout_ms == 2000
    assert e.roles.attribute == ""
    assert e.roles.scopes_as_roles and not e.roles.include_attributes
    assert e.roles.scopes == {"fleet.ops": ("ops.status", "ops.logs")}
    assert e.roles.tags == {"admin": ("ops.admin", "ops.logs")}
    assert e.issuer_delegation_file == "issuer.chain"
    assert e.issuer_lifetime_secs == 6 * 3600
    assert cfg._sources["enrollment"] == f"{p} [enrollment]"
    # Rebuilding from the dataclass is the identity.
    assert AsterConfig(**cfg.to_dict()) == cfg


def test_enrollment_env_vars(monkeypatch):
    ns = "ef" * 32
    for var, value in {
        "ASTER_ENROLLMENT_AUDIENCE": "fleet",
        "ASTER_ENROLLMENT_FLEET_NAMESPACE": ns,
        "ASTER_ENROLLMENT_STATE_PATH": "/s.json",
        "ASTER_ENROLLMENT_REPLICAS": "a, b",
        "ASTER_ENROLLMENT_POLICY": '{"scope": "ops"}',
        "ASTER_ENROLLMENT_SYNC_TIMEOUT_MS": "1234",
        "ASTER_ENROLLMENT_ROLE_ATTRIBUTE": "team",
        "ASTER_ENROLLMENT_SCOPES_AS_ROLES": "true",
        "ASTER_ENROLLMENT_ISSUER_LIFETIME": "2h",
        "ASTER_ENROLLMENT_RENEW": "false",
    }.items():
        monkeypatch.setenv(var, value)
    cfg = AsterConfig.from_env()
    e = cfg.enrollment
    assert (e.audience, e.fleet_namespace, e.state_path) == ("fleet", ns, "/s.json")
    assert e.replicas == ("a", "b")
    assert e.policy == {"scope": "ops"}
    assert e.limits.sync_timeout_ms == 1234
    assert e.roles.attribute == "team" and e.roles.scopes_as_roles
    assert e.issuer_lifetime_secs == 7200 and e.renew is False
    assert cfg.is_enrollment_enabled
    assert cfg._sources["enrollment"] == "ASTER_ENROLLMENT_*"


def test_enrollment_kwargs_dict_or_dataclass():
    as_dict = AsterConfig(
        enrollment={
            "audience": "fleet",
            "policy": "require_registration",
            "limits": {"step_timeout_ms": 7},
            "roles": {"tags": {"admin": "ops.admin,ops.logs"}},
        }
    )
    e = as_dict.enrollment
    assert isinstance(e, EnrollmentConfig)
    assert e.policy == "require_registration"
    assert e.limits.step_timeout_ms == 7
    assert e.roles.tags == {"admin": ("ops.admin", "ops.logs")}
    assert AsterConfig(enrollment=e) == as_dict
    with pytest.raises(TypeError):
        AsterConfig(enrollment={"no_such_field": 1})
    with pytest.raises(ValueError):
        AsterConfig(enrollment={"issuer_lifetime_secs": 0})


def test_enrollment_enablement_rule():
    assert not AsterConfig().is_enrollment_enabled
    # The root alone does not enable it (coordination uses the root too).
    assert not AsterConfig(root_pubkey=PUB, root_seq=1).is_enrollment_enabled
    assert AsterConfig(enrollment={"audience": "x"}).is_enrollment_enabled
    assert AsterConfig(enrollment={"fleet_namespace": "ab" * 32}).is_enrollment_enabled
    assert AsterConfig(enrollment_credential_file="a.cred").is_enrollment_enabled
    assert AsterConfig(enrollment_credential_chain="aster-chain-v1~x").is_enrollment_enabled
    assert AsterConfig(enrollment={"enabled": True}).is_enrollment_enabled
    assert not AsterConfig(enrollment={"enabled": False, "audience": "x"}).is_enrollment_enabled


@pytest.mark.parametrize(
    "field", ["allow_all_consumers", "allow_all_producers", "enrollment_credential_iid"]
)
def test_retired_consumer_admission_settings_are_gone(field):
    with pytest.raises(TypeError):
        AsterConfig(**{field: True})
    assert not hasattr(AsterConfig(), field)


def test_retired_consumer_admission_toml_keys_open_nothing(tmp_path):
    p = _write(
        tmp_path,
        "aster.toml",
        '[trust]\nallow_all_consumers = true\nenrollment_credential_iid = "x"\n',
    )
    assert not AsterConfig.from_file(p).is_enrollment_enabled
