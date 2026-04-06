"""Security tests: verify all size caps, limits, and validation in aster.limits."""

from __future__ import annotations

import json
import struct

import pytest
import zstandard

from aster.limits import (
    HEX_FIELD_LENGTHS,
    LimitExceeded,
    MAX_ACL_LIST_SIZE,
    MAX_ADMISSION_PAYLOAD_SIZE,
    MAX_COLLECTION_INDEX_ENTRIES,
    MAX_DECOMPRESSED_SIZE,
    MAX_FRAME_SIZE,
    MAX_MANIFEST_FIELDS_PER_METHOD,
    MAX_MANIFEST_METHODS,
    MAX_MANIFEST_TYPE_HASHES,
    MAX_METADATA_ENTRIES,
    MAX_METADATA_TOTAL_BYTES,
    MAX_SERVICES_IN_ADMISSION,
    MAX_STATUS_MESSAGE_LEN,
    validate_hex_field,
    validate_metadata,
    validate_status_message,
)


# ── validate_hex_field ────────────────────────────────────────────────────────


class TestHexFieldValidation:
    def test_valid_root_pubkey(self):
        validate_hex_field("root_pubkey", "ab" * 32)  # 64 hex chars

    def test_invalid_root_pubkey_too_short(self):
        with pytest.raises(LimitExceeded):
            validate_hex_field("root_pubkey", "ab" * 16)

    def test_invalid_root_pubkey_too_long(self):
        with pytest.raises(LimitExceeded):
            validate_hex_field("root_pubkey", "ab" * 64)

    def test_valid_signature(self):
        validate_hex_field("signature", "cd" * 64)  # 128 hex chars

    def test_invalid_signature_length(self):
        with pytest.raises(LimitExceeded):
            validate_hex_field("signature", "cd" * 32)

    def test_invalid_hex_characters(self):
        with pytest.raises(ValueError):
            validate_hex_field("root_pubkey", "zz" * 32)

    def test_empty_is_allowed(self):
        validate_hex_field("root_pubkey", "")  # optional field

    def test_valid_nonce(self):
        validate_hex_field("nonce", "ef" * 32)

    def test_valid_contract_id(self):
        validate_hex_field("contract_id", "01" * 32)

    def test_unknown_field_no_length_check(self):
        # Unknown field names don't have length requirements
        validate_hex_field("unknown_field", "abcd")


# ── validate_metadata ─────────────────────────────────────────────────────────


class TestMetadataValidation:
    def test_valid_metadata(self):
        validate_metadata(["key1", "key2"], ["val1", "val2"])

    def test_too_many_entries(self):
        keys = [f"k{i}" for i in range(MAX_METADATA_ENTRIES + 1)]
        values = [f"v{i}" for i in range(MAX_METADATA_ENTRIES + 1)]
        with pytest.raises(LimitExceeded, match="metadata entries"):
            validate_metadata(keys, values)

    def test_key_too_long(self):
        with pytest.raises(LimitExceeded, match="metadata key"):
            validate_metadata(["x" * 300], ["v"])

    def test_value_too_long(self):
        with pytest.raises(LimitExceeded, match="metadata value"):
            validate_metadata(["k"], ["x" * 5000])

    def test_total_bytes_exceeded(self):
        # Many entries that individually pass but collectively exceed 8KB total
        keys = [f"key{i:04d}" for i in range(60)]
        values = ["x" * 200 for _ in range(60)]  # 60*8 + 60*200 = 12480 > 8192
        with pytest.raises(LimitExceeded, match="metadata total"):
            validate_metadata(keys, values)

    def test_empty_metadata(self):
        validate_metadata([], [])


# ── validate_status_message ───────────────────────────────────────────────────


class TestStatusMessageValidation:
    def test_short_message_unchanged(self):
        assert validate_status_message("OK") == "OK"

    def test_long_message_truncated(self):
        msg = "x" * (MAX_STATUS_MESSAGE_LEN + 100)
        result = validate_status_message(msg)
        assert len(result) == MAX_STATUS_MESSAGE_LEN
        assert result.endswith("...")

    def test_exact_limit(self):
        msg = "x" * MAX_STATUS_MESSAGE_LEN
        assert validate_status_message(msg) == msg


# ── C1: Decompression bomb protection ────────────────────────────────────────


class TestDecompressionBomb:
    def test_normal_decompression(self):
        from aster.codec import ForyCodec
        from aster.types import SerializationMode

        codec = ForyCodec(mode=SerializationMode.XLANG)
        data = b"x" * 1000
        compressed = codec.compress(data)
        result = codec.decompress(compressed)
        assert result == data

    def test_max_output_size_enforced(self):
        """Verify that decompression beyond MAX_DECOMPRESSED_SIZE is rejected."""
        from aster.codec import ForyCodec
        from aster.types import SerializationMode

        codec = ForyCodec(mode=SerializationMode.XLANG)
        # Create a payload significantly larger than the limit
        large_data = b"\x00" * (MAX_DECOMPRESSED_SIZE * 2)
        cctx = zstandard.ZstdCompressor()
        bomb = cctx.compress(large_data)
        # The bomb is tiny but decompresses to 2x limit → must be rejected
        assert len(bomb) < 2000  # compressed should be tiny
        with pytest.raises(LimitExceeded, match="decompressed payload"):
            codec.decompress(bomb)


# ── C2: Frame read timeout ───────────────────────────────────────────────────


class TestFrameReadTimeout:
    def test_read_frame_has_timeout_parameter(self):
        """Verify read_frame accepts a timeout_s parameter."""
        import inspect
        from aster.framing import read_frame

        sig = inspect.signature(read_frame)
        assert "timeout_s" in sig.parameters

    def test_default_timeout_from_limits(self):
        from aster.limits import DEFAULT_FRAME_READ_TIMEOUT_S
        assert DEFAULT_FRAME_READ_TIMEOUT_S > 0
        assert DEFAULT_FRAME_READ_TIMEOUT_S <= 60


# ── C3: Metadata cap in server ────────────────────────────────────────────────


class TestServerMetadataCap:
    def test_validated_metadata_import(self):
        """Verify _validated_metadata helper exists in server."""
        from aster.server import _validated_metadata
        result = _validated_metadata(["k1"], ["v1"])
        assert result == {"k1": "v1"}

    def test_validated_metadata_none(self):
        from aster.server import _validated_metadata
        assert _validated_metadata(None, None) is None
        assert _validated_metadata([], []) is None

    def test_validated_metadata_truncates(self):
        from aster.server import _validated_metadata
        keys = [f"k{i}" for i in range(200)]
        values = [f"v{i}" for i in range(200)]
        result = _validated_metadata(keys, values)
        assert len(result) <= MAX_METADATA_ENTRIES


# ── H1: Admission services cap ───────────────────────────────────────────────


class TestAdmissionServicesCap:
    def test_services_capped(self):
        from aster.trust.consumer import ConsumerAdmissionResponse

        # Build a response with too many services
        services = [
            {"name": f"svc{i}", "version": 1, "contract_id": "ab" * 32, "channels": {}}
            for i in range(MAX_SERVICES_IN_ADMISSION + 10)
        ]
        raw = json.dumps({
            "admitted": True,
            "services": services,
            "registry_ticket": "",
            "root_pubkey": "",
        })
        resp = ConsumerAdmissionResponse.from_json(raw)
        assert len(resp.services) == MAX_SERVICES_IN_ADMISSION


# ── H2: RpcStatus message cap ────────────────────────────────────────────────


class TestRpcStatusMessageCap:
    def test_message_truncated(self):
        long_msg = "E" * 10000
        result = validate_status_message(long_msg)
        assert len(result) <= MAX_STATUS_MESSAGE_LEN


# ── H3: Collection index cap ─────────────────────────────────────────────────


class TestCollectionIndexCap:
    def test_limits_constant_reasonable(self):
        assert MAX_COLLECTION_INDEX_ENTRIES >= 100
        assert MAX_COLLECTION_INDEX_ENTRIES <= 100_000


# ── H4: Hex field validation in credentials ──────────────────────────────────


class TestCredentialHexValidation:
    def test_valid_credential(self):
        from aster.trust.consumer import consumer_cred_from_json

        cred_json = json.dumps({
            "credential_type": "policy",
            "root_pubkey": "ab" * 32,
            "expires_at": 9999999999,
            "attributes": {},
            "endpoint_id": "cd" * 32,
            "nonce": "ef" * 32,
            "signature": "01" * 64,
        })
        cred = consumer_cred_from_json(cred_json)
        assert len(cred.root_pubkey) == 32
        assert len(cred.signature) == 64

    def test_invalid_pubkey_length_rejected(self):
        from aster.trust.consumer import consumer_cred_from_json

        cred_json = json.dumps({
            "credential_type": "policy",
            "root_pubkey": "ab" * 16,  # too short
            "expires_at": 9999999999,
        })
        with pytest.raises(LimitExceeded):
            consumer_cred_from_json(cred_json)


# ── M1: ACL type validation ──────────────────────────────────────────────────


class TestACLValidation:
    def test_limits_constant(self):
        assert MAX_ACL_LIST_SIZE >= 100


# ── M3: Manifest validation ──────────────────────────────────────────────────


class TestManifestValidation:
    def test_methods_capped(self):
        from aster.contract.manifest import ContractManifest

        data = {
            "service": "test",
            "version": 1,
            "contract_id": "ab" * 32,
            "methods": [{"name": f"m{i}", "pattern": "unary"} for i in range(MAX_MANIFEST_METHODS + 10)],
        }
        manifest = ContractManifest.from_json(json.dumps(data))
        assert len(manifest.methods) == MAX_MANIFEST_METHODS

    def test_type_hashes_capped(self):
        from aster.contract.manifest import ContractManifest

        data = {
            "service": "test",
            "version": 1,
            "contract_id": "ab" * 32,
            "type_hashes": ["ab" * 32] * (MAX_MANIFEST_TYPE_HASHES + 10),
        }
        manifest = ContractManifest.from_json(json.dumps(data))
        assert len(manifest.type_hashes) == MAX_MANIFEST_TYPE_HASHES

    def test_version_coerced_to_int(self):
        from aster.contract.manifest import ContractManifest

        data = {"service": "test", "version": "3", "contract_id": "ab" * 32}
        manifest = ContractManifest.from_json(json.dumps(data))
        assert manifest.version == 3
        assert isinstance(manifest.version, int)


# ── Limits module consistency ─────────────────────────────────────────────────


class TestLimitsConsistency:
    def test_all_hex_fields_have_even_length(self):
        for name, length in HEX_FIELD_LENGTHS.items():
            assert length % 2 == 0, f"{name} has odd hex length {length}"

    def test_max_frame_size_is_16mb(self):
        assert MAX_FRAME_SIZE == 16 * 1024 * 1024

    def test_decompressed_size_matches_frame(self):
        assert MAX_DECOMPRESSED_SIZE == MAX_FRAME_SIZE

    def test_metadata_limits_reasonable(self):
        assert MAX_METADATA_ENTRIES <= 1000
        assert MAX_METADATA_TOTAL_BYTES <= 1024 * 1024
