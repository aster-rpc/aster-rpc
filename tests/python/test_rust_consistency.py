"""
Consistency tests: Python implementation vs Rust core.

For each function, calls both Python and Rust with identical inputs
and asserts byte-equal output. This validates the Rust extraction
before the Python code can be retired.
"""
import json
import struct
import pytest
import aster._aster as _aster

# Access the contract submodule from the native extension
_contract = _aster.contract
compute_contract_id_from_json = _contract.compute_contract_id_from_json
canonical_bytes_from_json = _contract.canonical_bytes_from_json
compute_type_hash = _contract.compute_type_hash
encode_frame = _contract.encode_frame
decode_frame = _contract.decode_frame
canonical_signing_bytes_from_json = _contract.canonical_signing_bytes_from_json
canonical_json = _contract.canonical_json


class TestCanonicalBytesConsistency:
    """Python canonical_xlang_bytes == Rust canonical_bytes_from_json."""

    def test_empty_service_contract(self):
        """Minimal ServiceContract (vector A.2)."""
        from aster.contract.identity import ServiceContract, ScopeKind, canonical_xlang_bytes

        # Python
        sc = ServiceContract(
            name="EmptyService",
            version=1,
            methods=[],
            serialization_modes=["xlang"],
            scoped=ScopeKind.SHARED,
            requires=None,
        )
        py_bytes = canonical_xlang_bytes(sc)

        # Rust (via JSON)
        sc_json = json.dumps({
            "name": "EmptyService",
            "version": 1,
            "methods": [],
            "serialization_modes": ["xlang"],
            "scoped": "shared",
            "requires": None,
        })
        rust_bytes = canonical_bytes_from_json("ServiceContract", sc_json)

        assert py_bytes == rust_bytes, f"Python: {py_bytes.hex()}\nRust:   {rust_bytes.hex()}"

    def test_enum_type_def(self):
        """Minimal TypeDef enum (vector A.3)."""
        from aster.contract.identity import (
            TypeDef, TypeDefKind, EnumValueDef, canonical_xlang_bytes
        )

        td = TypeDef(
            kind=TypeDefKind.ENUM,
            package="test",
            name="Color",
            fields=[],
            enum_values=[
                EnumValueDef(name="RED", value=0),
                EnumValueDef(name="GREEN", value=1),
                EnumValueDef(name="BLUE", value=2),
            ],
            union_variants=[],
        )
        py_bytes = canonical_xlang_bytes(td)

        td_json = json.dumps({
            "kind": "enum",
            "package": "test",
            "name": "Color",
            "fields": [],
            "enum_values": [
                {"name": "RED", "value": 0},
                {"name": "GREEN", "value": 1},
                {"name": "BLUE", "value": 2},
            ],
            "union_variants": [],
        })
        rust_bytes = canonical_bytes_from_json("TypeDef", td_json)

        assert py_bytes == rust_bytes

    def test_type_def_with_ref(self):
        """TypeDef with type reference (vector A.4)."""
        from aster.contract.identity import (
            TypeDef, TypeDefKind, FieldDef, TypeKind, ContainerKind,
            canonical_xlang_bytes
        )

        ref_hash = b"\xaa" * 32
        td = TypeDef(
            kind=TypeDefKind.MESSAGE,
            package="test",
            name="Wrapper",
            fields=[FieldDef(
                id=1, name="inner", type_kind=TypeKind.REF,
                type_primitive="", type_ref=ref_hash,
                self_ref_name="", optional=False, ref_tracked=False,
                container=ContainerKind.NONE,
                container_key_kind=TypeKind.PRIMITIVE,
                container_key_primitive="",
                container_key_ref=b"",
            )],
            enum_values=[],
            union_variants=[],
        )
        py_bytes = canonical_xlang_bytes(td)

        td_json = json.dumps({
            "kind": "message",
            "package": "test",
            "name": "Wrapper",
            "fields": [{
                "id": 1, "name": "inner", "type_kind": "ref",
                "type_primitive": "", "type_ref": "aa" * 32,
                "self_ref_name": "", "optional": False, "ref_tracked": False,
                "container": "none",
                "container_key_kind": "primitive",
                "container_key_primitive": "",
                "container_key_ref": "",
            }],
            "enum_values": [],
            "union_variants": [],
        })
        rust_bytes = canonical_bytes_from_json("TypeDef", td_json)

        assert py_bytes == rust_bytes

    def test_method_def_with_requires(self):
        """MethodDef with requires present (vector A.5)."""
        from aster.contract.identity import (
            MethodDef, MethodPattern, CapabilityRequirement, CapabilityKind,
            canonical_xlang_bytes
        )

        md = MethodDef(
            name="do_work",
            pattern=MethodPattern.UNARY,
            request_type=b"\x11" * 32,
            response_type=b"\x22" * 32,
            idempotent=True,
            default_timeout=30.0,
            requires=CapabilityRequirement(
                kind=CapabilityKind.ANY_OF,
                roles=["Admin", "Operator"],
            ),
        )
        py_bytes = canonical_xlang_bytes(md)

        md_json = json.dumps({
            "name": "do_work",
            "pattern": "unary",
            "request_type": "11" * 32,
            "response_type": "22" * 32,
            "idempotent": True,
            "default_timeout": 30.0,
            "requires": {
                "kind": "any_of",
                "roles": ["Admin", "Operator"],
            },
        })
        rust_bytes = canonical_bytes_from_json("MethodDef", md_json)

        assert py_bytes == rust_bytes

    def test_method_def_without_requires(self):
        """MethodDef with requires absent (vector A.6)."""
        from aster.contract.identity import (
            MethodDef, MethodPattern, canonical_xlang_bytes
        )

        md = MethodDef(
            name="do_work",
            pattern=MethodPattern.UNARY,
            request_type=b"\x11" * 32,
            response_type=b"\x22" * 32,
            idempotent=False,
            default_timeout=0.0,
            requires=None,
        )
        py_bytes = canonical_xlang_bytes(md)

        md_json = json.dumps({
            "name": "do_work",
            "pattern": "unary",
            "request_type": "11" * 32,
            "response_type": "22" * 32,
            "idempotent": False,
            "default_timeout": 0.0,
            "requires": None,
        })
        rust_bytes = canonical_bytes_from_json("MethodDef", md_json)

        assert py_bytes == rust_bytes


class TestContractIdConsistency:
    """Python compute_contract_id == Rust compute_contract_id_from_json."""

    def test_empty_service(self):
        from aster.contract.identity import (
            ServiceContract, ScopeKind, canonical_xlang_bytes, compute_contract_id
        )

        sc = ServiceContract(
            name="EmptyService", version=1, methods=[],
            serialization_modes=["xlang"], scoped=ScopeKind.SHARED, requires=None,
        )
        py_id = compute_contract_id(canonical_xlang_bytes(sc))

        sc_json = json.dumps({
            "name": "EmptyService", "version": 1, "methods": [],
            "serialization_modes": ["xlang"], "scoped": "shared", "requires": None,
        })
        rust_id = compute_contract_id_from_json(sc_json)

        assert py_id == rust_id


class TestTypeHashConsistency:
    """Python BLAKE3 == Rust BLAKE3."""

    def test_hash_matches(self):
        import blake3
        data = b"test data for hashing"
        py_hash = blake3.blake3(data).digest()
        rust_hash = compute_type_hash(data)
        assert py_hash == rust_hash


class TestFramingConsistency:
    """Python framing == Rust framing."""

    def test_encode_simple(self):
        """Encode a simple frame and compare."""
        payload = b"hello world"
        flags = 0x04  # HEADER

        # Python: manually construct expected bytes
        frame_body_len = 1 + len(payload)
        py_frame = struct.pack("<I", frame_body_len) + bytes([flags]) + payload

        rust_frame = encode_frame(payload, flags)
        assert py_frame == rust_frame

    def test_roundtrip(self):
        """Encode then decode."""
        payload = b"test payload"
        flags = 0x02  # TRAILER

        frame = encode_frame(payload, flags)
        decoded_payload, decoded_flags, consumed = decode_frame(frame)

        assert decoded_payload == payload
        assert decoded_flags == flags
        assert consumed == len(frame)

    def test_empty_trailer(self):
        """Empty payload with TRAILER flag."""
        frame = encode_frame(b"", 0x02)
        payload, flags, consumed = decode_frame(frame)
        assert payload == b""
        assert flags == 0x02


class TestSigningBytesConsistency:
    """Python canonical_signing_bytes == Rust canonical_signing_bytes_from_json."""

    def test_producer_credential(self):
        from aster.trust.signing import canonical_signing_bytes
        from aster.trust.credentials import EnrollmentCredential

        root_pubkey = bytes(range(32))
        cred = EnrollmentCredential(
            endpoint_id="abc123",
            root_pubkey=root_pubkey,
            expires_at=1700000000,
            attributes={"aster.role": "producer", "aster.name": "test"},
            signature=b"",
        )
        py_bytes = canonical_signing_bytes(cred)

        cred_json = json.dumps({
            "kind": "producer",
            "endpoint_id": "abc123",
            "root_pubkey": root_pubkey.hex(),
            "expires_at": 1700000000,
            "attributes": {"aster.role": "producer", "aster.name": "test"},
        })
        rust_bytes = canonical_signing_bytes_from_json(cred_json)

        assert py_bytes == rust_bytes

    def test_consumer_credential_policy(self):
        from aster.trust.signing import canonical_signing_bytes
        from aster.trust.credentials import ConsumerEnrollmentCredential

        root_pubkey = bytes(range(32))
        cred = ConsumerEnrollmentCredential(
            credential_type="policy",
            root_pubkey=root_pubkey,
            expires_at=1700000000,
            attributes={"aster.role": "consumer"},
            endpoint_id=None,
            nonce=None,
            signature=b"",
        )
        py_bytes = canonical_signing_bytes(cred)

        cred_json = json.dumps({
            "kind": "consumer",
            "credential_type": "policy",
            "root_pubkey": root_pubkey.hex(),
            "expires_at": 1700000000,
            "attributes": {"aster.role": "consumer"},
            "endpoint_id": None,
            "nonce": None,
        })
        rust_bytes = canonical_signing_bytes_from_json(cred_json)

        assert py_bytes == rust_bytes

    def test_consumer_credential_ott(self):
        from aster.trust.signing import canonical_signing_bytes
        from aster.trust.credentials import ConsumerEnrollmentCredential

        root_pubkey = bytes(range(32))
        nonce = bytes(range(32, 64))
        cred = ConsumerEnrollmentCredential(
            credential_type="ott",
            root_pubkey=root_pubkey,
            expires_at=1700000000,
            attributes={"aster.role": "consumer"},
            endpoint_id="node123",
            nonce=nonce,
            signature=b"",
        )
        py_bytes = canonical_signing_bytes(cred)

        cred_json = json.dumps({
            "kind": "consumer",
            "credential_type": "ott",
            "root_pubkey": root_pubkey.hex(),
            "expires_at": 1700000000,
            "attributes": {"aster.role": "consumer"},
            "endpoint_id": "node123",
            "nonce": nonce.hex(),
        })
        rust_bytes = canonical_signing_bytes_from_json(cred_json)

        assert py_bytes == rust_bytes


class TestCanonicalJsonConsistency:
    """Python canonical_json == Rust canonical_json."""

    def test_sorted_keys(self):
        from aster.trust.signing import canonical_json as py_canonical_json

        attrs = {"b": "2", "a": "1", "c": "3"}
        py_bytes = py_canonical_json(attrs)

        rust_bytes = canonical_json(json.dumps(attrs))
        assert py_bytes == rust_bytes

    def test_empty(self):
        from aster.trust.signing import canonical_json as py_canonical_json

        py_bytes = py_canonical_json({})
        rust_bytes = canonical_json("{}")
        assert py_bytes == rust_bytes
