"""Registry key schema: byte-exact with ``core/src/registry.rs`` and
prefix-free (iroh-docs lets a write at a key supersede the same author's older
entries whose keys extend it, so no key may be a prefix of another)."""

from __future__ import annotations

import pytest

from aster.registry.keys import (
    REGISTRY_PREFIXES,
    acl_key,
    channel_key,
    config_key,
    contract_key,
    decode_key_segment,
    key_segment,
    lease_key,
    lease_prefix,
    manifest_key,
    parse_version_key,
    tag_key,
    version_key,
    version_prefix,
)


def test_key_format_matches_core():
    assert contract_key("abc") == b"contracts/abc/"
    assert manifest_key("abc") == b"manifests/abc/"
    assert version_key("svc", 7) == b"services/svc/versions/v7/"
    assert version_prefix("svc") == b"services/svc/versions/"
    assert channel_key("svc", "stable") == b"services/svc/channels/stable/"
    assert tag_key("svc", "latest") == b"services/svc/tags/latest/"
    assert lease_key("svc", "cid", "eid") == b"services/svc/contracts/cid/endpoints/eid/"
    assert lease_prefix("svc", "cid") == b"services/svc/contracts/cid/endpoints/"
    assert acl_key("writers") == b"_aster/acl/writers/"
    assert config_key("version") == b"_aster/config/version/"


def test_segments_escape_slash_and_percent():
    assert key_segment("a/b%c") == "a%2Fb%25c"
    assert decode_key_segment("a%2Fb%25c") == "a/b%c"
    assert decode_key_segment("%252F") == "%2F"
    assert channel_key("x", "a/b") == b"services/x/channels/a%2Fb/"
    for bad in ["a/b", "%", "%2", "%41", "50%"]:
        assert decode_key_segment(bad) is None, bad


def test_version_keys_parse_strictly():
    assert parse_version_key(version_key("a/b", 10)) == ("a/b", 10)
    assert parse_version_key(version_key("svc", -3)) == ("svc", -3)
    for bad in [
        b"services/svc/versions/v1",
        b"services/svc/versions/v01/",
        b"services/svc/versions/v+1/",
        b"services/svc/versions/v-0/",
        b"services/svc/versions/1/",
        b"services/svc/versions/v1/x/",
        b"services/a/b/versions/v1/",
        b"services/svc/channels/v1/",
        b"services/svc/versions/v2147483648/",
    ]:
        assert parse_version_key(bad) is None, bad


def _family_keys(name: str) -> set[bytes]:
    """Keys from every builder, over segments chosen to provoke prefix
    collisions. A new key family belongs here."""
    leaves = ["1", "10", "stable", "stable2", "x/y", "x", "", "%2F"]
    keys = set()
    for leaf in leaves:
        keys |= {contract_key(leaf), manifest_key(leaf), acl_key(leaf), config_key(leaf)}
        keys |= {channel_key(name, leaf), tag_key(name, leaf)}
        keys |= {lease_key(name, leaf, eid) for eid in leaves}
    keys |= {version_key(name, v) for v in (1, 10, 100, -1)}
    return keys


NAMES = ["svc", "svc2", "a/b", "a", "50%"]


@pytest.mark.parametrize("name", NAMES)
def test_registry_keys_are_prefix_free(name):
    keys = _family_keys(name)
    for a in keys:
        for b in keys:
            assert a == b or not b.startswith(a), (a, b)


def test_registry_prefixes_cover_every_family():
    """A client that syncs only REGISTRY_PREFIXES receives every family the
    builders write (the same key set as the prefix-free check)."""
    for name in NAMES:
        for key in _family_keys(name):
            assert any(key.startswith(p) for p in REGISTRY_PREFIXES), key
