"""Explicit anonymous listener policy and shared native certificate helpers."""
from __future__ import annotations

import datetime
import hashlib
import ssl

import pytest
from aioquic.asyncio import connect
from aioquic.h3.connection import H3_ALPN
from aioquic.quic.configuration import QuicConfiguration
from cryptography import x509
from cryptography.hazmat.primitives import serialization, hashes
from cryptography.hazmat.primitives.asymmetric import ec, ed25519
from cryptography.x509.oid import NameOID

from aster import (AsterServer, GeneratedCertificate, generate_self_signed_certificate,
                   generate_webtransport_certificate, verify_certificate_binding,
                   service, rpc)
from aster.interceptors.base import CallContext
from aster.status import StatusCode
from test_http_listener import Echo, Request, Response, WTClient, request_http, assert_released

pytestmark = [pytest.mark.network, pytest.mark.timeout(30)]


def test_generated_certificate_native_material_and_binding():
    secret = bytes(range(32))
    node_id = ed25519.Ed25519PrivateKey.from_private_bytes(secret).public_key().public_bytes(
        serialization.Encoding.Raw, serialization.PublicFormat.Raw).hex()
    for cert in (generate_self_signed_certificate(["localhost", "127.0.0.1"], secret),
                 generate_webtransport_certificate("python-test", ["127.0.0.1", "example.test"], secret)):
        assert isinstance(cert, GeneratedCertificate)
        assert "PRIVATE KEY" not in repr(cert)
        parsed = x509.load_pem_x509_certificate(cert.certificate_pem)
        der = parsed.public_bytes(serialization.Encoding.DER)
        assert cert.sha256 == hashlib.sha256(der).digest() and len(cert.sha256) == 32
        key = serialization.load_pem_private_key(cert.private_key_pem, password=None)
        assert key.public_key().public_bytes(serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo) == parsed.public_key().public_bytes(serialization.Encoding.DER, serialization.PublicFormat.SubjectPublicKeyInfo)
        assert isinstance(key, ec.EllipticCurvePrivateKey) and isinstance(key.curve, ec.SECP256R1)
        verify_certificate_binding(cert.certificate_pem, node_id)
        # A binding authorizes SPKI, not all certificate fields: the same key
        # and signed SAN proof remain verifiable on a newly issued certificate.
        changed = (x509.CertificateBuilder().subject_name(parsed.subject).issuer_name(parsed.issuer)
            .public_key(key.public_key()).serial_number(x509.random_serial_number())
            .not_valid_before(parsed.not_valid_before_utc).not_valid_after(parsed.not_valid_after_utc)
            .add_extension(parsed.extensions.get_extension_for_class(x509.SubjectAlternativeName).value, False)
            .sign(key, hashes.SHA256()))
        assert changed.public_bytes(serialization.Encoding.DER) != der
        verify_certificate_binding(changed.public_bytes(serialization.Encoding.PEM), node_id)
        with pytest.raises(ValueError):
            verify_certificate_binding(cert.certificate_pem, "ff" * 32)
    sans = parsed.extensions.get_extension_for_class(x509.SubjectAlternativeName).value
    assert "example.test" in sans.get_values_for_type(x509.DNSName)
    assert "127.0.0.1" in [str(ip) for ip in sans.get_values_for_type(x509.IPAddress)]
    assert parsed.subject.get_attributes_for_oid(NameOID.COMMON_NAME)[0].value == "aster-node/python-test"
    assert parsed.not_valid_after_utc - parsed.not_valid_before_utc <= datetime.timedelta(days=14)
    unsigned = generate_self_signed_certificate()
    with pytest.raises(ValueError):
        verify_certificate_binding(unsigned.certificate_pem, node_id)
    for malformed in (b"", b"not a certificate"):
        with pytest.raises(ValueError):
            verify_certificate_binding(malformed, node_id)
    for malformed in (b"", b"x" * 31, b"x" * 33):
        with pytest.raises(ValueError):
            generate_self_signed_certificate(node_secret=malformed)
        with pytest.raises(ValueError):
            generate_webtransport_certificate("test", node_secret=malformed)


@service(name="PublicSession", version=1, scoped="session")
class PublicSession:
    created = 0

    def __init__(self, peer: str | None = None):
        type(self).created += 1

    @rpc()
    async def echo(self, request: Request, ctx: CallContext) -> Response:
        raise AssertionError("anonymous session reached Python factory/handler")


async def test_public_https_wt_anonymous_gates_sessions_and_cleanup():
    cert = generate_webtransport_certificate("public-python", ["127.0.0.1"])
    echo = Echo()
    async with AsterServer(services=[echo, PublicSession], allow_all_consumers=True) as server:
        listener = await server.start_public_http(certificate_pem=cert.certificate_pem,
            private_key_pem=cert.private_key_pem, webtransport=True)
        baseline = PublicSession.created
        assert listener.tcp_addr == listener.quic_addr
        try:
            for method, requests, expected in (
                ("echo", [Request("hello")], None),
                ("count", [Request("v")], [Response("v1"), Response("v2")]),
                ("collect", [Request("a"), Request("b")], [Response("ab")]),
                ("chat", [Request("a"), Request("b")], [Response("A"), Response("B")]),
            ):
                values, code = await request_http(listener.tcp_addr, method, "alice", requests)
                assert code == StatusCode.OK
                if expected is None:
                    assert values[0].text == "hello" and values[0].role == "" and not values[0].iroh
                    assert values[0].peer != "alice"
                else:
                    assert values == expected
            assert (await request_http(listener.tcp_addr, "secret", "alice"))[1] == StatusCode.PERMISSION_DENIED
            for sid in (None, "1", "spoofed"):
                assert (await request_http(listener.tcp_addr, service="PublicSession", session_id=sid))[1] == StatusCode.UNAUTHENTICATED
            config = QuicConfiguration(is_client=True, alpn_protocols=H3_ALPN)
            config.verify_mode = ssl.CERT_NONE
            host, port = listener.quic_addr.rsplit(":", 1)
            async with connect(host, int(port), configuration=config, create_protocol=WTClient) as client:
                session = await client.session(listener.quic_addr)
                values, code = await client.result(client.call(session, token="alice"))
                assert code == StatusCode.OK and values[0].role == "" and not values[0].iroh and values[0].peer != "alice"
                for method, requests, expected in (
                    ("count", [Request("v")], [Response("v1"), Response("v2")]),
                    ("collect", [Request("a"), Request("b")], [Response("ab")]),
                    ("chat", [Request("a"), Request("b")], [Response("A"), Response("B")]),
                ):
                    values, code = await client.result(client.call(session, method, "alice", requests))
                    assert code == StatusCode.OK and values == expected
                assert (await client.result(client.call(session, "secret", "alice")))[1] == StatusCode.PERMISSION_DENIED
                assert (await client.result(client.call(session, service="PublicSession", session_id=77)))[1] == StatusCode.UNAUTHENTICATED
                # Public mode is selected explicitly; passing no auth to start_http still fails.
                with pytest.raises(TypeError):
                    await server.start_http(authenticator=None, certificate_pem=cert.certificate_pem, private_key_pem=cert.private_key_pem)
            assert PublicSession.created == baseline
        finally:
            await listener.aclose()
        assert_released(listener.tcp_addr, True)
