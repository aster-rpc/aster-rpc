"""Tests for load_endpoint_config: TOML file + ASTER_* env var loading."""

import base64
import os
import textwrap
import pytest

from aster import load_endpoint_config, EndpointConfig


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _write_toml(tmp_path, content: str):
    p = tmp_path / "aster.toml"
    p.write_text(textwrap.dedent(content))
    return p


def _b64(b: bytes) -> str:
    return base64.b64encode(b).decode()


# ---------------------------------------------------------------------------
# Defaults
# ---------------------------------------------------------------------------

def test_defaults_no_args():
    cfg = load_endpoint_config()
    assert cfg.alpns == []
    assert cfg.relay_mode is None
    assert cfg.secret_key is None
    assert cfg.enable_monitoring is False
    assert cfg.enable_hooks is False
    assert cfg.hook_timeout_ms == 5000
    assert cfg.bind_addr is None
    assert cfg.clear_ip_transports is False
    assert cfg.clear_relay_transports is False
    assert cfg.portmapper_config is None
    assert cfg.proxy_url is None
    assert cfg.proxy_from_env is False


# ---------------------------------------------------------------------------
# TOML loading
# ---------------------------------------------------------------------------

def test_toml_basic(tmp_path):
    p = _write_toml(tmp_path, """
        alpns = ["myproto/1", "myproto/2"]
        relay_mode = "default"
        bind_addr = "0.0.0.0:9000"
        portmapper_config = "disabled"
        enable_monitoring = true
        hook_timeout_ms = 2000
    """)
    cfg = load_endpoint_config(p)
    assert cfg.alpns == [b"myproto/1", b"myproto/2"]
    assert cfg.relay_mode == "default"
    assert cfg.bind_addr == "0.0.0.0:9000"
    assert cfg.portmapper_config == "disabled"
    assert cfg.enable_monitoring is True
    assert cfg.hook_timeout_ms == 2000


def test_toml_secret_key(tmp_path):
    key = bytes(range(32))
    p = _write_toml(tmp_path, f'secret_key = "{_b64(key)}"')
    cfg = load_endpoint_config(p)
    assert cfg.secret_key == key


def test_toml_secret_key_null(tmp_path):
    p = _write_toml(tmp_path, "")
    cfg = load_endpoint_config(p)
    assert cfg.secret_key is None


def test_toml_bool_fields(tmp_path):
    p = _write_toml(tmp_path, """
        clear_ip_transports = true
        clear_relay_transports = false
        proxy_from_env = true
        enable_hooks = true
    """)
    cfg = load_endpoint_config(p)
    assert cfg.clear_ip_transports is True
    assert cfg.clear_relay_transports is False
    assert cfg.proxy_from_env is True
    assert cfg.enable_hooks is True


def test_toml_proxy_url(tmp_path):
    p = _write_toml(tmp_path, 'proxy_url = "http://proxy.corp:8080"')
    cfg = load_endpoint_config(p)
    assert cfg.proxy_url == "http://proxy.corp:8080"


def test_toml_file_not_found():
    with pytest.raises(FileNotFoundError):
        load_endpoint_config("/nonexistent/aster.toml")


def test_toml_malformed(tmp_path):
    p = tmp_path / "bad.toml"
    p.write_text("this is not = valid = toml = !!!")
    import sys
    if sys.version_info >= (3, 11):
        import tomllib
    else:
        import tomli as tomllib
    with pytest.raises(tomllib.TOMLDecodeError):
        load_endpoint_config(p)


def test_toml_invalid_alpns(tmp_path):
    p = _write_toml(tmp_path, "alpns = 42")
    with pytest.raises(ValueError, match="alpns"):
        load_endpoint_config(p)


def test_toml_invalid_bool_field(tmp_path):
    p = _write_toml(tmp_path, 'enable_monitoring = "yes"')
    with pytest.raises(ValueError, match="enable_monitoring"):
        load_endpoint_config(p)


def test_toml_invalid_hook_timeout(tmp_path):
    p = _write_toml(tmp_path, "hook_timeout_ms = -1")
    with pytest.raises(ValueError, match="hook_timeout_ms"):
        load_endpoint_config(p)


def test_toml_invalid_secret_key(tmp_path):
    p = _write_toml(tmp_path, 'secret_key = "not-valid-base64!!!"')
    with pytest.raises(ValueError, match="secret_key"):
        load_endpoint_config(p)


# ---------------------------------------------------------------------------
# Environment variable overrides
# ---------------------------------------------------------------------------

def test_env_alpns(monkeypatch):
    monkeypatch.setenv("ASTER_ALPNS", "proto/1,proto/2")
    cfg = load_endpoint_config()
    assert cfg.alpns == [b"proto/1", b"proto/2"]


def test_env_alpns_single(monkeypatch):
    monkeypatch.setenv("ASTER_ALPNS", "myproto/1")
    cfg = load_endpoint_config()
    assert cfg.alpns == [b"myproto/1"]


def test_env_relay_mode(monkeypatch):
    monkeypatch.setenv("ASTER_RELAY_MODE", "disabled")
    cfg = load_endpoint_config()
    assert cfg.relay_mode == "disabled"


def test_env_secret_key(monkeypatch):
    key = bytes(range(32))
    monkeypatch.setenv("ASTER_SECRET_KEY", _b64(key))
    cfg = load_endpoint_config()
    assert cfg.secret_key == key


def test_env_secret_key_invalid(monkeypatch):
    monkeypatch.setenv("ASTER_SECRET_KEY", "!!not-base64!!")
    with pytest.raises(ValueError, match="ASTER_SECRET_KEY"):
        load_endpoint_config()


def test_env_secret_key_empty_clears(monkeypatch, tmp_path):
    key = bytes(range(32))
    p = _write_toml(tmp_path, f'secret_key = "{_b64(key)}"')
    monkeypatch.setenv("ASTER_SECRET_KEY", "")
    cfg = load_endpoint_config(p)
    assert cfg.secret_key is None


def test_env_bool_true_variants(monkeypatch):
    for val in ("true", "True", "TRUE", "1", "yes", "on"):
        monkeypatch.setenv("ASTER_ENABLE_MONITORING", val)
        cfg = load_endpoint_config()
        assert cfg.enable_monitoring is True, f"failed for {val!r}"


def test_env_bool_false_variants(monkeypatch):
    for val in ("false", "False", "FALSE", "0", "no", "off"):
        monkeypatch.setenv("ASTER_ENABLE_MONITORING", val)
        cfg = load_endpoint_config()
        assert cfg.enable_monitoring is False, f"failed for {val!r}"


def test_env_bool_invalid(monkeypatch):
    monkeypatch.setenv("ASTER_ENABLE_MONITORING", "maybe")
    with pytest.raises(ValueError, match="ASTER_ENABLE_MONITORING"):
        load_endpoint_config()


def test_env_bind_addr(monkeypatch):
    monkeypatch.setenv("ASTER_BIND_ADDR", "127.0.0.1:0")
    cfg = load_endpoint_config()
    assert cfg.bind_addr == "127.0.0.1:0"


def test_env_bind_addr_empty_clears(monkeypatch, tmp_path):
    p = _write_toml(tmp_path, 'bind_addr = "0.0.0.0:9000"')
    monkeypatch.setenv("ASTER_BIND_ADDR", "")
    cfg = load_endpoint_config(p)
    assert cfg.bind_addr is None


def test_env_hook_timeout_ms(monkeypatch):
    monkeypatch.setenv("ASTER_HOOK_TIMEOUT_MS", "1000")
    cfg = load_endpoint_config()
    assert cfg.hook_timeout_ms == 1000


def test_env_hook_timeout_invalid(monkeypatch):
    monkeypatch.setenv("ASTER_HOOK_TIMEOUT_MS", "fast")
    with pytest.raises(ValueError, match="ASTER_HOOK_TIMEOUT_MS"):
        load_endpoint_config()


def test_env_clear_ip_transports(monkeypatch):
    monkeypatch.setenv("ASTER_CLEAR_IP_TRANSPORTS", "true")
    cfg = load_endpoint_config()
    assert cfg.clear_ip_transports is True


def test_env_portmapper_config(monkeypatch):
    monkeypatch.setenv("ASTER_PORTMAPPER_CONFIG", "disabled")
    cfg = load_endpoint_config()
    assert cfg.portmapper_config == "disabled"


def test_env_proxy_url(monkeypatch):
    monkeypatch.setenv("ASTER_PROXY_URL", "socks5://localhost:1080")
    cfg = load_endpoint_config()
    assert cfg.proxy_url == "socks5://localhost:1080"


def test_env_proxy_from_env(monkeypatch):
    monkeypatch.setenv("ASTER_PROXY_FROM_ENV", "1")
    cfg = load_endpoint_config()
    assert cfg.proxy_from_env is True


# ---------------------------------------------------------------------------
# Env overrides file (precedence)
# ---------------------------------------------------------------------------

def test_env_overrides_file(tmp_path, monkeypatch):
    p = _write_toml(tmp_path, """
        relay_mode = "default"
        bind_addr = "0.0.0.0:9000"
        portmapper_config = "enabled"
    """)
    monkeypatch.setenv("ASTER_RELAY_MODE", "disabled")
    monkeypatch.setenv("ASTER_BIND_ADDR", "127.0.0.1:0")
    cfg = load_endpoint_config(p)
    assert cfg.relay_mode == "disabled"       # env wins
    assert cfg.bind_addr == "127.0.0.1:0"     # env wins
    assert cfg.portmapper_config == "enabled"  # file value preserved


def test_env_alpns_overrides_file(tmp_path, monkeypatch):
    p = _write_toml(tmp_path, 'alpns = ["file-proto/1"]')
    monkeypatch.setenv("ASTER_ALPNS", "env-proto/1,env-proto/2")
    cfg = load_endpoint_config(p)
    assert cfg.alpns == [b"env-proto/1", b"env-proto/2"]


def test_env_secret_key_overrides_file(tmp_path, monkeypatch):
    file_key = bytes(range(32))
    env_key = bytes(reversed(range(32)))
    p = _write_toml(tmp_path, f'secret_key = "{_b64(file_key)}"')
    monkeypatch.setenv("ASTER_SECRET_KEY", _b64(env_key))
    cfg = load_endpoint_config(p)
    assert cfg.secret_key == env_key
