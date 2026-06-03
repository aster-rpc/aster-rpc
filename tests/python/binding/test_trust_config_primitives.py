import pytest

import aster


def test_namespace_capability_fory_round_trip_and_redaction():
    secret = bytes([0x33]) * 32
    namespace_id = aster.namespace_secret_id(secret)

    write = aster.NamespaceCapability.write(secret)
    assert write.kind == "write"
    assert write.can_write is True
    assert write.namespace_id() == namespace_id

    decoded = aster.NamespaceCapability.decode_fory(write.encode_fory())
    assert decoded.kind == "write"
    assert decoded.can_write is True
    assert decoded.namespace_id() == namespace_id
    assert decoded.namespace_secret() == secret
    assert "redacted" in repr(decoded)
    assert "333333" not in repr(decoded)

    read = aster.NamespaceCapability.read(namespace_id)
    decoded_read = aster.NamespaceCapability.decode_fory(read.encode_fory())
    assert decoded_read.kind == "read"
    assert decoded_read.can_write is False
    assert decoded_read.namespace_id() == namespace_id
    assert decoded_read.namespace_secret() is None
    assert "redacted" in repr(decoded_read)
    assert namespace_id.hex()[:12] not in repr(decoded_read)


def test_hpke_envelope_round_trip_with_associated_data():
    private_key, public_key = aster.hpke_generate_keypair()
    assert aster.hpke_public_key_from_private(private_key) == public_key

    aad = b"root-namespace/root-node/path/recipient/epoch/role/v1"
    plaintext = aster.NamespaceCapability.read(bytes([0x44]) * 32).encode_fory()

    envelope = aster.hpke_seal(public_key, aad, plaintext)
    assert envelope.alg == aster.hpke_envelope_alg()

    decoded = aster.HpkeEnvelope.decode_fory(envelope.encode_fory())
    assert aster.hpke_open(private_key, aad, decoded) == plaintext

    with pytest.raises(ValueError):
        aster.hpke_open(private_key, b"wrong-ad", decoded)


@pytest.mark.asyncio
async def test_ticket_free_namespace_import_open_helpers():
    node = await aster.IrohNode.memory()
    try:
        docs = aster.docs_client(node)
        secret = bytes([0x55]) * 32
        namespace_id = aster.namespace_secret_id(secret)

        doc = await docs.open_or_import_write_namespace(secret)
        assert bytes.fromhex(doc.doc_id()) == namespace_id

        reopened = await docs.open_namespace(namespace_id)
        assert reopened is not None
        assert reopened.doc_id() == doc.doc_id()

        read_doc = await docs.open_or_import_read_namespace(namespace_id)
        assert read_doc.doc_id() == doc.doc_id()

        missing = await docs.open_namespace(bytes([0x77]) * 32)
        assert missing is None
    finally:
        await node.shutdown()
